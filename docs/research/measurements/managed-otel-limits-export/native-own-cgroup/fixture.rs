//! Read-only native source integration, not a VM, transition or performance test.
use std::ffi::{CStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

pub use native_codec::{Failure, Limit, Scope, Snapshot};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::metrics::v1::metric;
use serde_json::{Value, json};

#[path = "../../managed-otel-rust-reference/src/reference.rs"]
mod reference;

#[derive(Debug, PartialEq, Eq)]
enum Limitation {
    Io {
        operation: &'static str,
        errno: Option<i32>,
    },
    Format(&'static str),
    Unavailable(&'static str),
    IdentityChanged,
    SemanticMismatch,
}

impl Limitation {
    fn value(&self) -> Value {
        match self {
            Self::Io { operation, errno } => json!({"type":"Io","operation":operation,"errno":errno}),
            Self::Format(field) => json!({"type":"Format","field":field}),
            Self::Unavailable(reason) => json!({"type":"Unavailable","reason":reason}),
            Self::IdentityChanged => json!({"type":"IdentityChanged"}),
            Self::SemanticMismatch => json!({"type":"SemanticMismatch"}),
        }
    }
}

fn io_failure(operation: &'static str) -> impl FnOnce(io::Error) -> Limitation {
    move |e| Limitation::Io {
        operation,
        errno: e.raw_os_error(),
    }
}

fn save(out: &Path, name: &str, bytes: &[u8]) -> Result<(), Limitation> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join(name))
        .map_err(io_failure("create_private_evidence"))?;
    file.write_all(bytes)
        .map_err(io_failure("write_private_evidence"))
}

fn save_json(out: &Path, name: &str, value: &Value) -> Result<(), Limitation> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| Limitation::Format("evidence_json"))?;
    save(out, name, &bytes)
}

fn absolute(bytes: &[u8]) -> Result<PathBuf, Limitation> {
    if bytes.contains(&0) {
        return Err(Limitation::Format("nul_in_path"));
    }
    let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Limitation::Unavailable("path_outside_visible_namespace"));
    }
    Ok(path)
}

fn own_path(bytes: &[u8]) -> Result<PathBuf, Limitation> {
    // proc_cgroup_show emits a namespace-relative, unescaped path. Refuse
    // ambiguous newline/mixed-hierarchy views rather than open a partial path.
    let body = bytes
        .strip_suffix(b"\n")
        .ok_or(Limitation::Format("cgroup_record"))?;
    if body.contains(&b'\n') || body.contains(&b'\r') {
        return Err(Limitation::Unavailable("multiple_or_ambiguous_cgroup_records"));
    }
    let path = body
        .strip_prefix(b"0::")
        .ok_or(Limitation::Unavailable("no_single_unified_entry"))?;
    if path.ends_with(b" (deleted)") {
        return Err(Limitation::Unavailable("deleted_cgroup"));
    }
    absolute(path)
}

fn unescape(bytes: &[u8]) -> Result<Vec<u8>, Limitation> {
    let mut result = Vec::new();
    let mut source = bytes.iter().copied();
    while let Some(byte) = source.next() {
        if byte != b'\\' {
            result.push(byte);
            continue;
        }
        let escaped = [source.next(), source.next(), source.next()];
        result.push(match escaped {
            [Some(b'0'), Some(b'4'), Some(b'0')] => b' ',
            [Some(b'0'), Some(b'1'), Some(b'1')] => b'\t',
            [Some(b'0'), Some(b'1'), Some(b'2')] => b'\n',
            [Some(b'1'), Some(b'3'), Some(b'4')] => b'\\',
            _ => return Err(Limitation::Format("mountinfo_escape")),
        });
    }
    Ok(result)
}

fn decimal(bytes: &[u8], field: &'static str) -> Result<u64, Limitation> {
    let text = std::str::from_utf8(bytes).map_err(|_| Limitation::Format(field))?;
    text.parse().map_err(|_| Limitation::Format(field))
}

#[derive(Debug, PartialEq, Eq)]
struct Mount {
    id: u64,
    major: u64,
    minor: u64,
    root: PathBuf,
    point: PathBuf,
}

fn resolve(path: &Path, bytes: &[u8]) -> Result<(Mount, PathBuf), Limitation> {
    let mut selected = None;
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split(|b| *b == b' ').collect();
        let separator = fields
            .iter()
            .position(|f| *f == b"-")
            .ok_or(Limitation::Format("mountinfo_separator"))?;
        let fs_at = separator
            .checked_add(1)
            .ok_or(Limitation::Format("mountinfo_fields"))?;
        if fields.get(fs_at).copied() != Some(b"cgroup2") {
            continue;
        }
        let get = |at| {
            fields
                .get(at)
                .copied()
                .ok_or(Limitation::Format("mountinfo_fields"))
        };
        let mut device = get(2)?.split(|b| *b == b':');
        let major = decimal(device.next().ok_or(Limitation::Format("major"))?, "major")?;
        let minor = decimal(device.next().ok_or(Limitation::Format("minor"))?, "minor")?;
        if device.next().is_some() {
            return Err(Limitation::Format("device"));
        }
        let mount = Mount {
            id: decimal(get(0)?, "mount_id")?,
            major,
            minor,
            root: absolute(&unescape(get(3)?)?)?,
            point: absolute(&unescape(get(4)?)?)?,
        };
        let Ok(relative) = path.strip_prefix(&mount.root) else {
            continue;
        };
        let location = mount.point.join(relative);
        if selected.replace((mount, location)).is_some() {
            return Err(Limitation::Unavailable("ambiguous_cgroup2_mount"));
        }
    }
    selected.ok_or(Limitation::Unavailable("own_cgroup_not_visible_on_cgroup2_mount"))
}

fn fd_set() -> io::Result<Vec<u32>> {
    let mut fds = Vec::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        let name = entry?.file_name();
        let n = name
            .to_str()
            .ok_or(io::ErrorKind::InvalidData)?
            .parse()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        fds.push(n);
    }
    fds.sort_unstable();
    Ok(fds)
}

fn fixed_raw(out: &Path, group: &File, name: &'static CStr, stem: &str) -> Result<Value, Limitation> {
    // SAFETY: fixed control name, held own-group directory, read-only open.
    let fd = unsafe {
        libc::openat(
            group.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Ok(json!({"type":"Io","errno":io::Error::last_os_error().raw_os_error()}));
    }
    // SAFETY: the fresh openat descriptor is owned here alone.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    match file.read_to_end(&mut bytes) {
        Ok(_) => {
            save(out, stem, &bytes)?;
            Ok(json!({"type":"Observed","raw_file":stem,"bytes":bytes.len()}))
        }
        Err(error) => {
            save(out, stem, &bytes)?;
            Ok(
                json!({"type":"Io","errno":error.raw_os_error(),"partial_raw_file":stem,"partial_bytes":bytes.len()}),
            )
        }
    }
}

fn failure(value: Failure) -> Value {
    match value {
        Failure::Missing => json!({"type":"Missing"}),
        Failure::Invalid => json!({"type":"Invalid"}),
        Failure::Overflow => json!({"type":"Overflow"}),
        Failure::Io(errno) => json!({"type":"Io","errno":errno}),
    }
}

fn limit(value: Result<Limit, Failure>) -> Value {
    match value {
        Ok(Limit::Finite(n)) => json!({"type":"Finite","value":n.to_string()}),
        Ok(Limit::Unbounded) => json!({"type":"Unbounded"}),
        Err(error) => json!({"type":"Failure","cause":failure(error)}),
    }
}

fn counter(value: Result<u64, Failure>) -> Value {
    match value {
        Ok(n) => json!({"type":"Observed","value":n.to_string()}),
        Err(error) => json!({"type":"Failure","cause":failure(error)}),
    }
}

fn observe(out: &Path, run: &str, vm: &str, generation: u64) -> Result<Value, Limitation> {
    let cgroup = fs::read("/proc/self/cgroup").map_err(io_failure("read_self_cgroup"))?;
    save(out, "proc-self-cgroup.before.raw", &cgroup)?;
    let mounts = fs::read("/proc/self/mountinfo").map_err(io_failure("read_self_mountinfo"))?;
    save(out, "proc-self-mountinfo.before.raw", &mounts)?;
    let own = own_path(&cgroup)?;
    let (mount, location) = resolve(&own, &mounts)?;
    let group = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&location)
        .map_err(io_failure("open_own_cgroup"))?;
    let metadata = group.metadata().map_err(io_failure("stat_own_cgroup"))?;
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: live held descriptor and writable statfs output storage.
    if unsafe { libc::fstatfs(group.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io_failure("fstatfs_own_cgroup")(io::Error::last_os_error()));
    }
    // SAFETY: successful fstatfs initialized the complete structure.
    let stat = unsafe { stat.assume_init() };
    if i128::from(stat.f_type) != i128::from(libc::CGROUP2_SUPER_MAGIC) {
        return Err(Limitation::Unavailable("held_directory_not_cgroup2"));
    }
    let fdinfo = fs::read(format!("/proc/self/fdinfo/{}", group.as_raw_fd()))
        .map_err(io_failure("read_own_fdinfo"))?;
    save(out, "held-group.fdinfo.raw", &fdinfo)?;
    let mut ids = fdinfo
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"mnt_id:\t"));
    let held_mount = decimal(
        ids.next().ok_or(Limitation::Format("fdinfo_mount_id"))?,
        "fdinfo_mount_id",
    )?;
    if ids.next().is_some()
        || held_mount != mount.id
        || u64::from(libc::major(metadata.dev())) != mount.major
        || u64::from(libc::minor(metadata.dev())) != mount.minor
    {
        return Err(Limitation::IdentityChanged);
    }
    save_json(
        out,
        "held-group.json",
        &json!({"mount_id":held_mount,"device":metadata.dev(),"inode":metadata.ino(),"filesystem_magic":i128::from(stat.f_type).to_string(),"own_cgroup_path_bytes":own.as_os_str().as_bytes(),"mount_root_bytes":mount.root.as_os_str().as_bytes(),"mount_point_bytes":mount.point.as_os_str().as_bytes(),"resolved_path_bytes":location.as_os_str().as_bytes()}),
    )?;
    let mut reader = native_codec::Reader::new().map_err(io_failure("reader_new"))?;
    let sample = reader
        .snapshot(&group, Scope::Workload)
        .map_err(io_failure("reader_snapshot"))?;
    save_json(
        out,
        "snapshot.json",
        &json!({"scope":"Workload (fixture label)","time_unix_nano":sample.time_unix_nano.to_string(),"cpu_usec":counter(sample.cpu_usec),"memory_bytes":counter(sample.memory_bytes),"threads":counter(sample.threads),"memory_limit":limit(sample.memory_limit),"task_limit":limit(sample.task_limit),"read_bytes":counter(sample.read_bytes),"write_bytes":counter(sample.write_bytes),"raw_reads_are_separate_nonatomic_observations":true}),
    )?;
    let memory_raw = fixed_raw(out, &group, c"memory.max", "memory.max.raw")?;
    let task_raw = fixed_raw(out, &group, c"pids.max", "pids.max.raw")?;
    save_json(
        out,
        "raw-limit-results.json",
        &json!({"memory.max":memory_raw,"pids.max":task_raw}),
    )?;
    let mut payload = Vec::new();
    native_codec::write(&mut payload, run, vm, generation, &[sample]).map_err(io_failure("native_encode"))?;
    save(out, "native.json", &payload)?;
    let expected =
        reference::request(run, vm, generation, &[sample]).map_err(io_failure("official_request"))?;
    let official = serde_json::to_vec(&expected).map_err(|_| Limitation::Format("official_serialize"))?;
    save(out, "official-compact.json", &official)?;
    let decoded: ExportMetricsServiceRequest =
        serde_json::from_slice(&payload).map_err(|_| Limitation::Format("official_decode_native"))?;
    let official_decoded: ExportMetricsServiceRequest =
        serde_json::from_slice(&official).map_err(|_| Limitation::Format("official_decode_generated"))?;
    if decoded != expected || official_decoded != expected {
        return Err(Limitation::SemanticMismatch);
    }
    let states = decoded
        .resource_metrics
        .iter()
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
        .filter(|m| m.name == "shards.cgroup.limit.state")
        .filter_map(|m| match &m.data {
            Some(metric::Data::Gauge(g)) => Some(g.data_points.len()),
            _ => None,
        })
        .sum::<usize>();
    if states != 6 {
        return Err(Limitation::SemanticMismatch);
    }
    let after_group = fs::read("/proc/self/cgroup").map_err(io_failure("read_self_cgroup_after"))?;
    save(out, "proc-self-cgroup.after.raw", &after_group)?;
    let after_mounts = fs::read("/proc/self/mountinfo").map_err(io_failure("read_self_mountinfo_after"))?;
    save(out, "proc-self-mountinfo.after.raw", &after_mounts)?;
    let after_own = own_path(&after_group)?;
    if after_own != own || resolve(&after_own, &after_mounts)? != (mount, location) {
        return Err(Limitation::IdentityChanged);
    }
    Ok(
        json!({"type":"Observed","full_official_typed_requests_equal":true,"limit_state_points":states,"memory_limit":limit(sample.memory_limit),"task_limit":limit(sample.task_limit),"identity_inputs_are_synthetic_fixture_labels":true,"atomic_snapshot_or_exclusive_process_accounting":false,"cap_writes_or_cgroup_creation":false}),
    )
}

fn run() -> Result<bool, Limitation> {
    let mut args = std::env::args_os().skip(1);
    let out = PathBuf::from(args.next().ok_or(Limitation::Format("output_argument"))?);
    let mut text = || {
        args.next()
            .and_then(|s| s.into_string().ok())
            .ok_or(Limitation::Format("fixture_identity_arguments"))
    };
    let run = text()?;
    let vm = text()?;
    let generation: u64 = text()?
        .parse()
        .map_err(|_| Limitation::Format("fixture_generation"))?;
    if args.next().is_some() {
        return Err(Limitation::Format("extra_arguments"));
    }
    let before = fd_set().map_err(io_failure("fd_before"))?;
    let outcome = observe(&out, &run, &vm, generation);
    let after = fd_set().map_err(io_failure("fd_after"))?;
    let same = before == after;
    let report = json!({"outcome":match &outcome { Ok(value) => value.clone(), Err(error) => error.value() },"fd_before":before,"fd_after":after,"fd_sets_equal":same,"fd_sets_include_the_temporary_enumeration_fd":true,"run_label":run,"vm_label":vm,"fixture_generation":generation});
    save_json(&out, "result.json", &report)?;
    let stdout = serde_json::to_vec(&report).map_err(|_| Limitation::Format("stdout_json"))?;
    io::stdout()
        .lock()
        .write_all(&stdout)
        .map_err(io_failure("stdout"))?;
    Ok(outcome.is_ok() && same)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{error:?}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod own_resolution_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn own_unified_path_is_not_unescaped_or_allowed_to_walk_up() {
        assert_eq!(own_path(b"0::/a\\040b\n").unwrap(), Path::new("/a\\040b"));
        for invalid in [
            b"0::/../outside\n".as_slice(),
            b"0::/a\n\n",
            b"0::/a\n0::/b\n",
            b"1:cpu:/a\n",
            b"0::/a (deleted)\n",
        ] {
            assert!(own_path(invalid).is_err());
        }
    }

    #[test]
    fn mount_mapping_uses_component_prefix_and_ignores_optional_tags() {
        let input = b"10 1 0:25 /owned /view\\040space rw unknown:yes - cgroup2 cgroup rw\n";
        let (mount, path) = resolve(Path::new("/owned/member"), input).unwrap();
        assert_eq!(mount.id, 10);
        assert_eq!(path, Path::new("/view space/member"));
        assert!(resolve(Path::new("/owned-other"), input).is_err());
        assert!(
            resolve(
                Path::new("/owned/member"),
                &[input.as_slice(), input.as_slice()].concat()
            )
            .is_err()
        );
    }

    #[test]
    fn missing_mount_and_malformed_escapes_are_limitations_without_fallback() {
        assert!(resolve(Path::new("/a"), b"10 1 0:1 / /sys rw - proc proc rw\n").is_err());
        assert!(resolve(Path::new("/a"), b"10 1 0:1 / /bad\\999 rw - cgroup2 cgroup rw\n").is_err());
        assert_eq!(unescape(b"a\\040b\\011c\\012d\\134e").unwrap(), b"a b\tc\nd\\e");
    }
}
