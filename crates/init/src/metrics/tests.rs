use super::otlp::CgroupEvent;
use super::{Failure, IoBytes, Limit, Reader, Scope, Snapshot};
use shards_testdir::TempDir;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt as _;

type LimitReader = fn(&mut Reader, &File) -> Result<Limit, Failure>;
const LIMIT_READERS: [(&str, LimitReader); 2] = [
    ("memory.max", Reader::memory_limit),
    ("pids.max", Reader::task_limit),
];

// These are private regular files with kernel-format contents, not real cgroups.
struct Group {
    held: File,
    directory: TempDir,
}

impl Group {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = TempDir::new("metrics-reader").unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        for (name, contents) in files {
            fs::write(directory.join(name), contents).unwrap();
        }
        let held = File::open(&directory).unwrap();
        Self { held, directory }
    }

    fn complete(cpu: &str, memory: &str, threads: &str, io: &str) -> Self {
        Self::new(&[
            ("cpu.stat", cpu),
            ("memory.current", memory),
            ("pids.current", threads),
            ("io.stat", io),
        ])
    }

    fn sample(self, reader: &mut Reader, scope: Scope) -> Snapshot {
        let snapshot = reader.snapshot(&self.held, scope).unwrap();
        assert_eq!(snapshot.scope, scope);
        assert_ne!(snapshot.time_unix_nano, 0);
        let path = self.directory.path().to_owned();
        // Field order closes the held directory before TempDir removes its files.
        drop(self);
        assert!(!path.exists(), "owned fixture directory remains: {path:?}");
        snapshot
    }
}

fn page_size() -> usize {
    // SAFETY: sysconf has no pointer arguments and queries this process's page size.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
    assert_ne!(page, 0);
    page
}

fn across_page(page: usize, prefix: &str, token: &str, suffix: &str) -> String {
    let start = page.checked_sub(token.len() / 2).unwrap();
    assert!(prefix.len() <= start);
    assert!(start < page && start + token.len() > page);
    format!("{prefix}{}{token}{suffix}", " ".repeat(start - prefix.len()))
}

fn assert_other_metrics(sample: Snapshot) {
    assert_eq!(sample.cpu_usec, Ok(31));
    assert_eq!(sample.memory_bytes, Ok(47));
    assert_eq!(sample.threads, Ok(3));
}

#[test]
fn kernel_fields_are_selected_by_key_and_scope_is_preserved() {
    let mut reader = Reader::new().unwrap();
    for scope in [Scope::Run, Scope::Workload, Scope::Member(u32::MAX)] {
        let sample = Group::complete(
            "system_usec 9\nfuture_stat not-a-number\nusage_usec 31\nuser_usec 22\n",
            "47\n",
            "3\n",
            "8:16 wios=3 wbytes=17 future=ignored rbytes=11\n8:0 rbytes=5 rios=1 wbytes=7",
        )
        .sample(&mut reader, scope);
        assert_other_metrics(sample);
        assert_eq!(sample.read_bytes, Ok(IoBytes::Counted(16)));
        assert_eq!(sample.write_bytes, Ok(IoBytes::Counted(24)));
    }
}

#[test]
fn empty_io_is_zero_and_missing_io_fails_unless_io_is_not_delegated() {
    let mut reader = Reader::new().unwrap();
    let empty = Group::complete("usage_usec 31\n", "47\n", "3\n", "").sample(&mut reader, Scope::Run);
    assert_other_metrics(empty);
    assert_eq!(empty.read_bytes, Ok(IoBytes::Counted(0)));
    assert_eq!(empty.write_bytes, Ok(IoBytes::Counted(0)));

    // Without io.stat, the cgroup's own listing decides, in every scope: one that leaves io
    // out explains the absence; one that names io, or none to read, leaves it a failure.
    let enoent = Err(Failure::Io(libc::ENOENT));
    for (scope, controllers, expected) in [
        (Scope::Member(7), Some("memory pids\n"), Ok(IoBytes::NotDelegated)),
        (
            Scope::Run,
            Some("cpuset cpu memory pids\n"),
            Ok(IoBytes::NotDelegated),
        ),
        (Scope::Workload, Some("cpuset cpu io memory pids\n"), enoent),
        (Scope::Member(7), Some("io memory pids\n"), enoent),
        (Scope::Member(7), None, enoent),
    ] {
        let mut files = vec![
            ("cpu.stat", "usage_usec 31\n"),
            ("memory.current", "47\n"),
            ("pids.current", "3\n"),
        ];
        files.extend(controllers.map(|listed| ("cgroup.controllers", listed)));
        let missing = Group::new(&files).sample(&mut reader, scope);
        assert_other_metrics(missing);
        assert_eq!(
            (missing.read_bytes, missing.write_bytes),
            (expected, expected),
            "{scope:?} {controllers:?}"
        );
    }

    // Only a missing file is explained by the listing: any other failure stays one.
    let group = Group::new(&[
        ("cgroup.controllers", "memory pids\n"),
        ("cpu.stat", "usage_usec 31\n"),
        ("memory.current", "47\n"),
        ("pids.current", "3\n"),
        ("other", "8:0 rbytes=1 wbytes=2\n"),
    ]);
    std::os::unix::fs::symlink("other", group.directory.join("io.stat")).unwrap();
    let symlink = group.sample(&mut reader, Scope::Member(7));
    assert_eq!(symlink.read_bytes, Err(Failure::Io(libc::ELOOP)));
    assert_eq!(symlink.write_bytes, Err(Failure::Io(libc::ELOOP)));
}

fn collection_error_count(text: &str) -> &str {
    let (_, rest) = text
        .split_once("\"name\":\"shards.cgroup.collection.error.count\"")
        .unwrap();
    let (_, rest) = rest.split_once("\"asInt\":\"").unwrap();
    rest.split_once('"').unwrap().0
}

#[test]
fn a_member_without_the_io_controller_reports_no_collection_error() {
    let mut reader = Reader::new().unwrap();
    // A domain member as the kernel makes it: its parent enables only pids and memory for
    // it (domains/admission.rs `enable`), so its cgroup.controllers lists those and it has
    // no io.* file (Linux cgroup-v2.rst, "Enabling and Disabling").
    let member = Group::new(&[
        ("cgroup.controllers", "memory pids\n"),
        ("cpu.stat", "usage_usec 31\n"),
        ("memory.current", "47\n"),
        ("pids.current", "3\n"),
        ("memory.max", "max\n"),
        ("pids.max", "max\n"),
        (
            "memory.events",
            "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n",
        ),
        ("pids.events", "max 0\n"),
    ])
    .sample(&mut reader, Scope::Member(7));
    let mut bytes = Vec::new();
    super::otlp::write(&mut bytes, "run", "vm", 0, &[member]).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(collection_error_count(&text), "0", "{text}");
    assert!(
        !text.contains("\"name\":\"shards.cgroup.collection.error\","),
        "{text}"
    );
}

#[test]
fn missing_cpu_usage_does_not_erase_independent_metrics() {
    let mut reader = Reader::new().unwrap();
    for cpu in ["", "user_usec 9\nsystem_usec 22\n"] {
        let sample = Group::complete(cpu, "47\n", "3\n", "8:0 rbytes=11 wbytes=17\n")
            .sample(&mut reader, Scope::Workload);
        assert_eq!(sample.cpu_usec, Err(Failure::Missing));
        assert_eq!(sample.memory_bytes, Ok(47));
        assert_eq!(sample.threads, Ok(3));
        assert_eq!(sample.read_bytes, Ok(IoBytes::Counted(11)));
        assert_eq!(sample.write_bytes, Ok(IoBytes::Counted(17)));
    }
}

#[test]
fn duplicate_selected_io_fields_on_one_device_are_invalid() {
    let mut reader = Reader::new().unwrap();
    for io in [
        "8:0 rbytes=11 wbytes=17 rbytes=11\n",
        "8:0 wbytes=17 rbytes=11 wbytes=19\n",
    ] {
        let sample = Group::complete("usage_usec 31\n", "47\n", "3\n", io).sample(&mut reader, Scope::Run);
        assert_other_metrics(sample);
        assert_eq!(sample.read_bytes, Err(Failure::Invalid));
        assert_eq!(sample.write_bytes, Err(Failure::Invalid));
    }
}

#[test]
fn device_lines_without_byte_fields_add_zero() {
    let mut reader = Reader::new().unwrap();
    // A device whose blkg has done no read or write is printed by name alone (Linux v6.18
    // block/blk-cgroup.c 1193-1227); the first is a live io.stat, byte for byte
    // (docs/research/measurements/audit-fix-init-v1/runs/cgroup-io-probe.log).
    let live = concat!(
        "253:0 \n",
        "259:0 rbytes=35000320 wbytes=0 rios=1309 wios=0 dbytes=0 dios=0\n",
        "259:3 rbytes=18190336 wbytes=0 rios=1162 wios=0 dbytes=0 dios=0\n",
        "259:2 rbytes=662855680 wbytes=0 rios=12719 wios=0 dbytes=0 dios=0\n",
        "259:1 rbytes=11812864 wbytes=0 rios=721 wios=0 dbytes=0 dios=0\n",
    );
    for (io, read, write) in [
        (live, 727_859_200, 0),
        ("8:0 \n", 0, 0),
        ("8:0 ", 0, 0),
        // iocost's own field on a device with no read or write (blk-iocost.c v6.18 3063).
        (
            "8:0  cost.usage=0\n8:16 rbytes=5 wbytes=7 rios=1 wios=1 dbytes=0 dios=0\n",
            5,
            7,
        ),
    ] {
        let sample = Group::complete("usage_usec 31\n", "47\n", "3\n", io).sample(&mut reader, Scope::Run);
        assert_other_metrics(sample);
        assert_eq!(
            (sample.read_bytes, sample.write_bytes),
            (Ok(IoBytes::Counted(read)), Ok(IoBytes::Counted(write))),
            "{io:?}"
        );
    }
}

#[test]
fn a_device_line_with_one_byte_field_is_missing() {
    let mut reader = Reader::new().unwrap();
    // The kernel prints rbytes and wbytes together or neither (blk-cgroup.c v6.18 1206-1210).
    for io in [
        "8:0 rbytes=5 rios=1\n",
        "8:0 wbytes=7 wios=1\n",
        "8:16 rbytes=5 wbytes=7\n8:0 rbytes=1\n",
    ] {
        let sample = Group::complete("usage_usec 31\n", "47\n", "3\n", io).sample(&mut reader, Scope::Run);
        assert_other_metrics(sample);
        assert_eq!(
            (sample.read_bytes, sample.write_bytes),
            (Err(Failure::Missing), Err(Failure::Missing)),
            "{io:?}"
        );
    }
}

#[test]
fn io_numbers_and_cross_device_sums_are_checked() {
    let mut reader = Reader::new().unwrap();
    let too_large = (u128::from(u64::MAX) + 1).to_string();
    for io in [
        format!("8:0 rbytes={too_large} wbytes=0\n"),
        format!("8:0 rbytes=0 wbytes={too_large}\n"),
        format!("8:0 rbytes={} wbytes=0\n8:1 rbytes=1 wbytes=0\n", u64::MAX),
        format!("8:0 rbytes=0 wbytes={}\n8:1 rbytes=0 wbytes=1\n", u64::MAX),
    ] {
        let sample = Group::complete("usage_usec 31\n", "47\n", "3\n", &io).sample(&mut reader, Scope::Run);
        assert_other_metrics(sample);
        assert_eq!(sample.read_bytes, Err(Failure::Overflow));
        assert_eq!(sample.write_bytes, Err(Failure::Overflow));
    }
}

#[test]
fn full_u64_values_are_not_narrowed_and_larger_numbers_overflow() {
    let mut reader = Reader::new().unwrap();
    let max = u64::MAX.to_string();
    let sample = Group::complete(
        &format!("usage_usec {max}\n"),
        &max,
        &max,
        &format!("8:0 rbytes={max} wbytes={max}\n"),
    )
    .sample(&mut reader, Scope::Run);
    for value in [sample.cpu_usec, sample.memory_bytes, sample.threads] {
        assert_eq!(value, Ok(u64::MAX));
    }
    for value in [sample.read_bytes, sample.write_bytes] {
        assert_eq!(value, Ok(IoBytes::Counted(u64::MAX)));
    }

    let too_large = (u128::from(u64::MAX) + 1).to_string();
    let sample = Group::complete(&format!("usage_usec {too_large}\n"), &too_large, &too_large, "")
        .sample(&mut reader, Scope::Run);
    assert_eq!(sample.cpu_usec, Err(Failure::Overflow));
    assert_eq!(sample.memory_bytes, Err(Failure::Overflow));
    assert_eq!(sample.threads, Err(Failure::Overflow));
    assert_eq!(sample.read_bytes, Ok(IoBytes::Counted(0)));
    assert_eq!(sample.write_bytes, Ok(IoBytes::Counted(0)));
}

#[test]
fn scalar_extra_tokens_are_invalid_across_whitespace_and_lines() {
    let mut reader = Reader::new().unwrap();
    for extra in ["47 48\n", "47\n48\n"] {
        let sample = Group::complete("usage_usec 31\n", extra, "3\n", "").sample(&mut reader, Scope::Run);
        assert_eq!(sample.memory_bytes, Err(Failure::Invalid));
        assert_eq!(sample.threads, Ok(3));
        assert_eq!(sample.cpu_usec, Ok(31));
        let sample = Group::complete("usage_usec 31\n", "47\n", extra, "").sample(&mut reader, Scope::Run);
        assert_eq!(sample.memory_bytes, Ok(47));
        assert_eq!(sample.threads, Err(Failure::Invalid));
        assert_eq!(sample.cpu_usec, Ok(31));
    }
}

#[test]
fn cpu_key_and_number_survive_an_actual_page_boundary() {
    let mut reader = Reader::new().unwrap();
    let page = page_size();
    for cpu in [
        across_page(page, "", "usage_usec", " 31\n"),
        across_page(page, "usage_usec ", "31", "\n"),
    ] {
        let sample = Group::complete(&cpu, "47\n", "3\n", "").sample(&mut reader, Scope::Run);
        assert_other_metrics(sample);
    }
}

#[test]
fn selected_io_tokens_survive_an_actual_page_boundary() {
    let mut reader = Reader::new().unwrap();
    let page = page_size();
    for io in [
        across_page(page, "8:0 ", "rbytes=11", " wbytes=17\n"),
        across_page(page, "8:0 rbytes=11 ", "wbytes=17", "\n"),
    ] {
        let sample = Group::complete("usage_usec 31\n", "47\n", "3\n", &io).sample(&mut reader, Scope::Run);
        assert_other_metrics(sample);
        assert_eq!(sample.read_bytes, Ok(IoBytes::Counted(11)));
        assert_eq!(sample.write_bytes, Ok(IoBytes::Counted(17)));
    }
}

#[test]
fn unknown_fields_longer_than_a_page_do_not_hide_selected_fields() {
    let mut reader = Reader::new().unwrap();
    let long = "x".repeat(page_size().checked_mul(2).unwrap());
    let cpu = format!("{long} ignored\nusage_usec 31\nfuture {long}\n");
    let io = format!("8:0 {long}=ignored rbytes=11 future={long} wbytes=17\n");
    let sample = Group::complete(&cpu, "47\n", "3\n", &io).sample(&mut reader, Scope::Run);
    assert_other_metrics(sample);
    assert_eq!(sample.read_bytes, Ok(IoBytes::Counted(11)));
    assert_eq!(sample.write_bytes, Ok(IoBytes::Counted(17)));
}

#[test]
fn limits_distinguish_finite_zero_full_u64_and_symbolic_max() {
    let mut reader = Reader::new().unwrap();
    let largest = u64::MAX.to_string();
    for (text, expected) in [
        ("0\n", Limit::Finite(0)),
        ("47", Limit::Finite(47)),
        (largest.as_str(), Limit::Finite(u64::MAX)),
        ("max\n", Limit::Unbounded),
        (" \tmax\r\n", Limit::Unbounded),
    ] {
        for (name, read) in LIMIT_READERS {
            let group = Group::new(&[(name, text)]);
            assert_eq!(read(&mut reader, &group.held), Ok(expected), "{name}: {text:?}");
        }
    }
}

#[test]
fn limits_missing_malformed_duplicate_and_overflow_values_stay_errors() {
    let mut reader = Reader::new().unwrap();
    let overflow = (u128::from(u64::MAX) + 1).to_string();
    let long = "9".repeat(super::TOKEN_BYTES.checked_add(1).unwrap());
    for (text, expected) in [
        ("", Failure::Missing),
        (" \t\r\n", Failure::Missing),
        ("MAX", Failure::Invalid),
        ("-1", Failure::Invalid),
        ("+1", Failure::Invalid),
        ("NaN", Failure::Invalid),
        ("ma x", Failure::Invalid),
        ("max 0", Failure::Invalid),
        ("0 max", Failure::Invalid),
        ("max\nmax", Failure::Invalid),
        (overflow.as_str(), Failure::Overflow),
        (long.as_str(), Failure::Overflow),
    ] {
        for (name, read) in LIMIT_READERS {
            let group = Group::new(&[(name, text)]);
            assert_eq!(read(&mut reader, &group.held), Err(expected), "{name}: {text:?}");
        }
    }
}

#[test]
fn limits_fixed_name_open_preserves_missing_and_symlink_errno() {
    let mut reader = Reader::new().unwrap();
    for (name, read) in LIMIT_READERS {
        let group = Group::new(&[("other", "47\n")]);
        assert_eq!(
            read(&mut reader, &group.held),
            Err(Failure::Io(libc::ENOENT)),
            "{name}"
        );
        std::os::unix::fs::symlink("other", group.directory.join(name)).unwrap();
        assert_eq!(
            read(&mut reader, &group.held),
            Err(Failure::Io(libc::ELOOP)),
            "{name}"
        );
        assert_eq!(fs::read(group.directory.join("other")).unwrap(), b"47\n");
    }
}

#[test]
fn limits_cross_pages_and_reuse_the_snapshot_buffer_after_errors() {
    let mut reader = Reader::new().unwrap();
    let page = page_size();
    let buffer = (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity());
    assert_eq!(buffer.1, page);
    let group = Group::complete("usage_usec 31\n", "47\n", "3\n", "8:0 rbytes=11 wbytes=17\n");
    let largest = u64::MAX.to_string();
    for (token, expected) in [
        (largest.as_str(), Ok(Limit::Finite(u64::MAX))),
        ("max", Ok(Limit::Unbounded)),
        ("invalid", Err(Failure::Invalid)),
        ("0", Ok(Limit::Finite(0))),
    ] {
        let text = if token.len() > 1 {
            across_page(page, "", token, "\n")
        } else {
            token.to_owned()
        };
        for (name, read) in LIMIT_READERS {
            fs::write(group.directory.join(name), &text).unwrap();
            assert_eq!(read(&mut reader, &group.held), expected, "{name}");
        }
        let snapshot = reader.snapshot(&group.held, Scope::Workload).unwrap();
        assert_other_metrics(snapshot);
        assert_eq!(snapshot.read_bytes, Ok(IoBytes::Counted(11)));
        assert_eq!(snapshot.write_bytes, Ok(IoBytes::Counted(17)));
        assert_eq!(
            (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity()),
            buffer
        );
    }
}

#[test]
fn limits_keep_fixed_names_and_held_groups_independent_after_errors() {
    let mut reader = Reader::new().unwrap();
    let buffer = (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity());
    let group = Group::new(&[("memory.max", "47\n"), ("pids.max", "3\n")]);
    let other = Group::new(&[("memory.max", "max\n"), ("pids.max", "invalid\n")]);
    assert_eq!(reader.memory_limit(&group.held), Ok(Limit::Finite(47)));
    assert_eq!(reader.task_limit(&group.held), Ok(Limit::Finite(3)));
    assert_eq!(reader.memory_limit(&other.held), Ok(Limit::Unbounded));
    assert_eq!(reader.task_limit(&other.held), Err(Failure::Invalid));
    fs::write(other.directory.join("pids.max"), "max\n").unwrap();
    assert_eq!(reader.task_limit(&other.held), Ok(Limit::Unbounded));
    assert_eq!(reader.task_limit(&group.held), Ok(Limit::Finite(3)));
    assert_eq!(reader.memory_limit(&group.held), Ok(Limit::Finite(47)));
    assert_eq!(
        (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity()),
        buffer
    );
}

#[test]
fn snapshots_keep_actual_limit_results_and_reuse_the_one_page() {
    let mut reader = Reader::new().unwrap();
    let buffer = (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity());
    let group = Group::complete("usage_usec 31\n", "47\n", "3\n", "8:0 rbytes=11 wbytes=17\n");
    let signed_max = i64::MAX.to_string();
    let unsigned_max = u64::MAX.to_string();
    for (text, expected) in [
        ("0", Ok(Limit::Finite(0))),
        (signed_max.as_str(), Ok(Limit::Finite(i64::MAX as u64))),
        (unsigned_max.as_str(), Ok(Limit::Finite(u64::MAX))),
        ("max", Ok(Limit::Unbounded)),
        ("malformed", Err(Failure::Invalid)),
        ("", Err(Failure::Missing)),
    ] {
        for name in ["memory.max", "pids.max"] {
            fs::write(group.directory.join(name), text).unwrap();
        }
        let sample = reader.snapshot(&group.held, Scope::Workload).unwrap();
        assert_other_metrics(sample);
        assert_eq!(sample.memory_limit, expected);
        assert_eq!(sample.task_limit, expected);
        assert_eq!(
            (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity()),
            buffer
        );
    }
    fs::write(group.directory.join("memory.max"), "0").unwrap();
    fs::remove_file(group.directory.join("pids.max")).unwrap();
    let missing = reader.snapshot(&group.held, Scope::Member(7)).unwrap();
    assert_eq!(missing.memory_limit, Ok(Limit::Finite(0)));
    assert_eq!(missing.task_limit, Err(Failure::Io(libc::ENOENT)));
    std::os::unix::fs::symlink("memory.max", group.directory.join("pids.max")).unwrap();
    let symlink = reader.snapshot(&group.held, Scope::Run).unwrap();
    assert_eq!(symlink.memory_limit, Ok(Limit::Finite(0)));
    assert_eq!(symlink.task_limit, Err(Failure::Io(libc::ELOOP)));
    assert_eq!(fs::read(group.directory.join("memory.max")).unwrap(), b"0");
}

#[test]
fn event_keys_are_order_independent_and_unknown_fields_are_not_labels() {
    let mut reader = Reader::new().unwrap();
    for scope in [Scope::Run, Scope::Workload, Scope::Member(u32::MAX)] {
        let sample = Group::new(&[
            (
                "memory.events",
                "oom_group_kill 5\nhigh 1\nfuture not-a-number\noom_kill 4\nlow 0\noom 3\nmax 2",
            ),
            ("pids.events", "future ignored\nlow 999\nmax 6\n"),
        ])
        .sample(&mut reader, scope);
        assert_eq!(sample.events, [Ok(0), Ok(1), Ok(2), Ok(3), Ok(4), Ok(5), Ok(6)]);
    }
}

#[test]
fn event_parse_failures_do_not_erase_independent_selected_fields() {
    let mut reader = Reader::new().unwrap();
    let sample = Group::new(&[
        (
            "memory.events",
            "low 0\nhigh -1\nmax\noom 18446744073709551616\noom_kill 4\noom_kill 4\noom_group_kill 5\n",
        ),
        ("pids.events", "max 6 extra\n"),
    ])
    .sample(&mut reader, Scope::Run);
    assert_eq!(
        sample.events,
        [
            Ok(0),
            Err(Failure::Invalid),
            Err(Failure::Invalid),
            Err(Failure::Overflow),
            Err(Failure::Invalid),
            Ok(5),
            Err(Failure::Invalid)
        ]
    );
    let sample = Group::new(&[("memory.events", "low 0\nfuture 7\n"), ("pids.events", "")])
        .sample(&mut reader, Scope::Run);
    assert_eq!(
        sample.events,
        [
            Ok(0),
            Err(Failure::Missing),
            Err(Failure::Missing),
            Err(Failure::Missing),
            Err(Failure::Missing),
            Err(Failure::Missing),
            Err(Failure::Missing)
        ]
    );
}

#[test]
fn event_reader_keeps_u64_range_before_codec_checks_signed_bounds() {
    let mut reader = Reader::new().unwrap();
    for value in [0, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        let memory = format!(
            "low {value}\nhigh {value}\nmax {value}\noom {value}\noom_kill {value}\noom_group_kill {value}\n"
        );
        let tasks = format!("max {value}\n");
        let sample = Group::new(&[("memory.events", &memory), ("pids.events", &tasks)])
            .sample(&mut reader, Scope::Workload);
        assert_eq!(sample.events, [Ok(value); CgroupEvent::ALL.len()]);
    }
    let long = "9".repeat(super::TOKEN_BYTES + 1);
    for text in [
        "max -1\n".to_owned(),
        "max NaN\n".to_owned(),
        "max 1\nmax 2\n".to_owned(),
        "max 18446744073709551616\n".to_owned(),
        format!("max {long}\n"),
    ] {
        let expected = if text.contains(&long) || text.contains("18446744073709551616") {
            Failure::Overflow
        } else {
            Failure::Invalid
        };
        let sample = Group::new(&[("memory.events", "low 1\n"), ("pids.events", &text)])
            .sample(&mut reader, Scope::Run);
        assert_eq!(sample.events.first(), Some(&Ok(1)));
        assert_eq!(sample.events.last(), Some(&Err(expected)));
    }
}

#[test]
fn event_tokens_cross_pages_and_long_unknown_lines_cannot_hide_later_known_keys() {
    let mut reader = Reader::new().unwrap();
    let page = page_size();
    let long = "x".repeat(page.checked_mul(2).unwrap());
    for memory in [
        across_page(
            page,
            "",
            "oom_group_kill",
            " 5\nlow 0\nhigh 1\nmax 2\noom 3\noom_kill 4\n",
        ),
        across_page(
            page,
            "oom_group_kill ",
            "18446744073709551615",
            "\nlow 0\nhigh 1\nmax 2\noom 3\noom_kill 4\n",
        ),
        format!(
            "low 0\nmax{long} 999\nhigh 1\nfuture {long} max 999\nmax 2\noom 3\noom_kill 4\noom_group_kill 5\n"
        ),
    ] {
        let expected_group = if memory.contains("18446744073709551615") {
            u64::MAX
        } else {
            5
        };
        let tasks = format!("max{long} 999\nfuture {long} max 999\nmax 6\n");
        let sample = Group::new(&[("memory.events", &memory), ("pids.events", &tasks)])
            .sample(&mut reader, Scope::Run);
        assert_eq!(
            sample.events,
            [Ok(0), Ok(1), Ok(2), Ok(3), Ok(4), Ok(expected_group), Ok(6)]
        );
    }
}

#[test]
fn event_file_failures_keep_exact_errno_and_the_other_file_independent() {
    let mut reader = Reader::new().unwrap();
    let group = Group::new(&[("pids.events", "max 0\n"), ("other", "low 99\n")]);
    for cause in [libc::ENOENT, libc::ELOOP, libc::EISDIR] {
        if cause == libc::ELOOP {
            std::os::unix::fs::symlink("other", group.directory.join("memory.events")).unwrap();
        } else if cause == libc::EISDIR {
            fs::remove_file(group.directory.join("memory.events")).unwrap();
            fs::create_dir(group.directory.join("memory.events")).unwrap();
        }
        let sample = reader.snapshot(&group.held, Scope::Run).unwrap();
        for (event, value) in CgroupEvent::ALL.into_iter().zip(sample.events) {
            assert_eq!(
                value,
                if event == CgroupEvent::TaskMax {
                    Ok(0)
                } else {
                    Err(Failure::Io(cause))
                }
            );
        }
        assert_eq!(fs::read(group.directory.join("other")).unwrap(), b"low 99\n");
    }
}

#[test]
fn event_snapshots_reuse_one_page_and_keep_held_groups_independent_after_errors() {
    let mut reader = Reader::new().unwrap();
    let buffer = (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity());
    let group = Group::new(&[
        (
            "memory.events",
            "low 0\nhigh 1\nmax 2\noom 3\noom_kill 4\noom_group_kill 5\n",
        ),
        ("pids.events", "max 6\n"),
    ]);
    let other = Group::new(&[("memory.events", "low invalid\n"), ("pids.events", "max 7\n")]);
    let first = reader.snapshot(&group.held, Scope::Workload).unwrap();
    let failed = reader.snapshot(&other.held, Scope::Member(7)).unwrap();
    assert_eq!(failed.events.first(), Some(&Err(Failure::Invalid)));
    assert_eq!(failed.events.last(), Some(&Ok(7)));
    let again = reader.snapshot(&group.held, Scope::Workload).unwrap();
    assert_eq!(first.events, again.events);
    assert_eq!(again.scope, Scope::Workload);
    fs::write(other.directory.join("memory.events"), "low 9\n").unwrap();
    assert_eq!(
        reader
            .snapshot(&other.held, Scope::Member(7))
            .unwrap()
            .events
            .first(),
        Some(&Ok(9))
    );
    assert_eq!(
        (reader.bytes.as_ptr(), reader.bytes.len(), reader.bytes.capacity()),
        buffer
    );
}
