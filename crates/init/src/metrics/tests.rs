use super::{Failure, Reader, Scope, Snapshot};
use shards_testdir::TempDir;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt as _;

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
        assert_eq!(sample.read_bytes, Ok(16));
        assert_eq!(sample.write_bytes, Ok(24));
    }
}

#[test]
fn empty_io_is_zero_but_missing_io_remains_an_io_failure() {
    let mut reader = Reader::new().unwrap();
    let empty = Group::complete("usage_usec 31\n", "47\n", "3\n", "").sample(&mut reader, Scope::Run);
    assert_other_metrics(empty);
    assert_eq!(empty.read_bytes, Ok(0));
    assert_eq!(empty.write_bytes, Ok(0));

    let missing = Group::new(&[
        ("cpu.stat", "usage_usec 31\n"),
        ("memory.current", "47\n"),
        ("pids.current", "3\n"),
    ])
    .sample(&mut reader, Scope::Member(7));
    assert_other_metrics(missing);
    assert_eq!(missing.read_bytes, Err(Failure::Io(libc::ENOENT)));
    assert_eq!(missing.write_bytes, Err(Failure::Io(libc::ENOENT)));
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
        assert_eq!(sample.read_bytes, Ok(11));
        assert_eq!(sample.write_bytes, Ok(17));
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
    for value in [
        sample.cpu_usec,
        sample.memory_bytes,
        sample.threads,
        sample.read_bytes,
        sample.write_bytes,
    ] {
        assert_eq!(value, Ok(u64::MAX));
    }

    let too_large = (u128::from(u64::MAX) + 1).to_string();
    let sample = Group::complete(&format!("usage_usec {too_large}\n"), &too_large, &too_large, "")
        .sample(&mut reader, Scope::Run);
    assert_eq!(sample.cpu_usec, Err(Failure::Overflow));
    assert_eq!(sample.memory_bytes, Err(Failure::Overflow));
    assert_eq!(sample.threads, Err(Failure::Overflow));
    assert_eq!(sample.read_bytes, Ok(0));
    assert_eq!(sample.write_bytes, Ok(0));
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
        assert_eq!(sample.read_bytes, Ok(11));
        assert_eq!(sample.write_bytes, Ok(17));
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
    assert_eq!(sample.read_bytes, Ok(11));
    assert_eq!(sample.write_bytes, Ok(17));
}
