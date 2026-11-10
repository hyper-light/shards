//! Coordinator-executed real-VM fixture. Implementer checks never execute this test.
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

mod parse;

#[path = "../managed-allocation-metrics/native_faults.rs"]
mod native_faults;

struct NativeFaultSample {
    sample: native_faults::Sample,
    failure: Option<ObservationFailure>,
    query_failure: Option<ObservationFailure>,
}

impl NativeFaultSample {
    fn unavailable(
        expected: Option<shards_ipc::Named>,
        reason: native_faults::Unknown,
        failure: Option<ObservationFailure>,
    ) -> Self {
        let identity =
            expected.and_then(|n| n.start.map(|start| native_faults::Identity { pid: n.pid, start }));
        Self {
            sample: native_faults::Sample::unknown(native_faults::Source::this_host(), identity, reason),
            failure,
            query_failure: None,
        }
    }

    fn probe(identity: &PinnedProcess) -> Self {
        let expected = Some(identity.expected);
        let Some(start) = identity.expected.start else {
            return Self::unavailable(
                expected,
                native_faults::Unknown::MissingIdentity,
                Some(ObservationFailure::MissingExpectedStart),
            );
        };
        if let Err(error) = identity.check() {
            return Self::unavailable(expected, native_faults::Unknown::IdentityUnverified, Some(error));
        }
        let usage =
            shards_vmm::platform::process_usage(identity.expected.pid).map_err(ObservationFailure::io);
        if let Err(error) = identity.check() {
            let mut outcome =
                Self::unavailable(expected, native_faults::Unknown::IdentityUnverified, Some(error));
            outcome.query_failure = usage.err();
            return outcome;
        }
        match usage {
            Ok(usage) => Self {
                sample: native_faults::Sample::observed(
                    native_faults::Source::this_host(),
                    native_faults::Identity {
                        pid: identity.expected.pid,
                        start,
                    },
                    usage.faults,
                ),
                failure: None,
                query_failure: None,
            },
            Err(error) => {
                let mut outcome = Self::unavailable(expected, native_faults::Unknown::ReadUnavailable, None);
                outcome.query_failure = Some(error);
                outcome
            }
        }
    }

    fn parent(expected: &Result<shards_ipc::Named, ObservationFailure>) -> Self {
        match expected {
            Ok(expected) => match PinnedProcess::open(*expected) {
                Ok(identity) => Self::probe(&identity),
                Err(error) => Self::unavailable(
                    Some(*expected),
                    native_faults::Unknown::ReadUnavailable,
                    Some(error),
                ),
            },
            Err(error) => {
                Self::unavailable(None, native_faults::Unknown::MissingIdentity, Some(error.clone()))
            }
        }
        // The temporary self directory drops before any later FD checkpoint.
    }

    fn host(host: &HostExit) -> Self {
        match host {
            HostExit::Watching { identity, .. } => Self::probe(identity),
            HostExit::Unavailable {
                expected, failure, ..
            } => Self::unavailable(
                *expected,
                native_faults::Unknown::IdentityUnverified,
                Some(failure.clone()),
            ),
            HostExit::ExitReady(expected) | HostExit::Deadline(expected) => Self::unavailable(
                Some(*expected),
                native_faults::Unknown::ProcessExitedOrHandleReleased,
                None,
            ),
        }
    }

    fn record(&self) -> Value {
        json!({"source":self.sample.source.label(),"native_bits":self.sample.source.native_bits(),"native_caveat":self.sample.source.caveat(),"expected_identity":self.sample.identity.map(|n| (n.pid,n.start)),"atomic":false,"observation":match self.sample.observation {
            native_faults::Observation::ObservedBestEffort(value)=>json!({"state":"ObservedBestEffort","value":value,"identity_validation":"matched before and after existing platform query; not a coherent snapshot or quiescence proof"}),
            native_faults::Observation::Unknown(reason)=>json!({"state":"Unknown","reason":format!("{reason:?}")}),
        },"failure":self.failure.as_ref().map(ObservationFailure::record),"query_failure":self.query_failure.as_ref().map(ObservationFailure::record)})
    }
}

struct NativeFaultPhase {
    observer_parent: NativeFaultSample,
    daemon: NativeFaultSample,
    vm: NativeFaultSample,
}

impl NativeFaultPhase {
    fn sample(exits: Option<&ExitProof>, parent: &Result<shards_ipc::Named, ObservationFailure>) -> Self {
        let unavailable = || {
            NativeFaultSample::unavailable(
                None,
                native_faults::Unknown::MissingIdentity,
                Some(ObservationFailure::NotPrepared),
            )
        };
        Self {
            observer_parent: NativeFaultSample::parent(parent),
            daemon: exits
                .map(|p| NativeFaultSample::host(&p.daemon))
                .unwrap_or_else(unavailable),
            vm: exits
                .map(|p| NativeFaultSample::host(&p.vm))
                .unwrap_or_else(unavailable),
        }
    }

    fn after_cleanup() -> Self {
        // This process is still ours. No numeric daemon/VM lookup after their
        // pinned objects are released, and no exited-process zero is invented.
        let parent = shards_ipc::started(std::process::id())
            .map(|start| shards_ipc::Named {
                pid: std::process::id(),
                start: Some(start),
            })
            .map_err(ObservationFailure::io);
        let released = || {
            NativeFaultSample::unavailable(None, native_faults::Unknown::ProcessExitedOrHandleReleased, None)
        };
        Self {
            observer_parent: NativeFaultSample::parent(&parent),
            daemon: released(),
            vm: released(),
        }
    }

    fn record(&self) -> Value {
        json!({"observer_parent":self.observer_parent.record(),"daemon":self.daemon.record(),"vm":self.vm.record()})
    }
}

#[derive(Default)]
struct NativeFaultPhases {
    after_start_and_watch_acquisition: Option<NativeFaultPhase>,
    owner_ready_before_recording: Option<NativeFaultPhase>,
    after_proc_recording: Option<NativeFaultPhase>,
    after_observer_completion: Option<NativeFaultPhase>,
    after_cleanup: Option<NativeFaultPhase>,
}

impl NativeFaultPhases {
    fn record(&self) -> Value {
        let phase = |value: &Option<NativeFaultPhase>| {
            value
                .as_ref()
                .map(NativeFaultPhase::record)
                .unwrap_or_else(|| json!({"state":"NotReached"}))
        };
        let first = self.after_start_and_watch_acquisition.as_ref();
        let ready = self.owner_ready_before_recording.as_ref();
        json!({"scope":"native per-process counters through existing platform::process_usage; observer, owned daemon, VM host separately; no guest/kernel/process-tree completeness","semantics":"non-atomic, synchronous platform probes allocate/read as fixture costs; native categories and widths remain distinct; no quiescence; differences conditional on absent wrap/reset/lossy fallback","after_start_and_watch_acquisition":phase(&self.after_start_and_watch_acquisition),"owner_ready_before_recording":phase(&self.owner_ready_before_recording),"after_proc_recording":phase(&self.after_proc_recording),"after_observer_completion":phase(&self.after_observer_completion),"after_cleanup":phase(&self.after_cleanup),"start_to_ready":{
            "observer_parent":native_fault_delta(first.map(|p| &p.observer_parent),ready.map(|p| &p.observer_parent)),
            "daemon":native_fault_delta(first.map(|p| &p.daemon),ready.map(|p| &p.daemon)),
            "vm":native_fault_delta(first.map(|p| &p.vm),ready.map(|p| &p.vm)),
        }})
    }
}

fn native_fault_delta(before: Option<&NativeFaultSample>, after: Option<&NativeFaultSample>) -> Value {
    let (Some(before), Some(after)) = (before, after) else {
        return json!({"state":"Unknown","reason":"PhaseNotReached"});
    };
    let delta = native_faults::between(before.sample, after.sample, true);
    let mut record = json!({"source":before.sample.source.label(),"native_bits":before.sample.source.native_bits(),"identity":before.sample.identity.map(|n| (n.pid,n.start)),"caveat":"no established absence of native wrap/reset or existing platform parser/conversion fallback; conditional difference, never an exact universal fault count"});
    match delta {
        native_faults::Delta::ConditionalDifference(value) => {
            record["state"] = json!("ConditionalDifference");
            record["value"] = json!(value);
        }
        native_faults::Delta::Unknown(reason) => {
            record["state"] = json!("Unknown");
            record["reason"] = json!(format!("{reason:?}"));
        }
    }
    record
}

#[derive(Default)]
struct StartupArtifactAllocations {
    before_resolution: Option<managed_alloc::Snapshot>,
    after_resolution: Option<managed_alloc::Snapshot>,
    artifact_value_ready: Option<managed_alloc::Snapshot>,
}

impl StartupArtifactAllocations {
    fn record(&self) -> Value {
        json!({
            "before_resolution":allocation_snapshot_record(self.before_resolution),
            "after_resolution":allocation_snapshot_record(self.after_resolution),
            "artifact_value_ready":allocation_snapshot_record(self.artifact_value_ready),
        })
    }
}

struct StartupAllocations {
    test_entered: managed_alloc::Snapshot,
    supplied_inputs_ready: Option<managed_alloc::Snapshot>,
    fixture: StartupArtifactAllocations,
    cli: StartupArtifactAllocations,
    testguest: StartupArtifactAllocations,
    kernel: StartupArtifactAllocations,
    init: StartupArtifactAllocations,
    inputs_recorded: Option<managed_alloc::Snapshot>,
}

impl StartupAllocations {
    fn new() -> Self {
        Self {
            test_entered: MANAGED_ALLOC.snapshot(),
            supplied_inputs_ready: None,
            fixture: StartupArtifactAllocations::default(),
            cli: StartupArtifactAllocations::default(),
            testguest: StartupArtifactAllocations::default(),
            kernel: StartupArtifactAllocations::default(),
            init: StartupArtifactAllocations::default(),
            inputs_recorded: None,
        }
    }

    fn record(&self) -> Value {
        json!({
            "scope":"observer/test parent Rust wrapper only; all threads and fixture/instrumentation included",
            "coverage_start":"first Rust test-body statement; earlier libtest/loader/setup work remains outside these intervals",
            "semantics":"ConcurrentNonAtomic cumulative snapshots and recorded process-lifetime peak; no quiescence, exact attribution, phase peak or native/process-tree completeness",
            "artifact_boundary":"artifact_value_ready precedes outer inputs-JSON conversion/insertion; intervals include original resolver/read/hash/value costs, not exclusive call-site allocation attribution",
            "test_entered":allocation_snapshot_record(Some(self.test_entered)),
            "supplied_inputs_ready":allocation_snapshot_record(self.supplied_inputs_ready),
            "fixture":self.fixture.record(),
            "cli":self.cli.record(),
            "testguest":self.testguest.record(),
            "kernel":self.kernel.record(),
            "init":self.init.record(),
            "inputs_recorded":allocation_snapshot_record(self.inputs_recorded),
        })
    }
}

struct AllocationPhases {
    before_fixture: managed_alloc::Snapshot,
    after_start_and_watch_acquisition: Option<managed_alloc::Snapshot>,
    owner_ready_before_recording: Option<managed_alloc::Snapshot>,
    after_proc_recording: Option<managed_alloc::Snapshot>,
    after_observer_completion: Option<managed_alloc::Snapshot>,
    after_shutdown_watch_release_and_fd_sample: Option<managed_alloc::Snapshot>,
}

impl AllocationPhases {
    fn new() -> Self {
        Self {
            before_fixture: MANAGED_ALLOC.snapshot(),
            after_start_and_watch_acquisition: None,
            owner_ready_before_recording: None,
            after_proc_recording: None,
            after_observer_completion: None,
            after_shutdown_watch_release_and_fd_sample: None,
        }
    }

    fn record(&self) -> Value {
        json!({
            "scope":"observer/test parent only; all threads using this Rust global wrapper; includes fixture and instrumentation costs",
            "excludes":"daemon/VM/guest, direct System/native/C-library allocations, usable sizes/RSS/kernel/process-tree completeness",
            "semantics":"ConcurrentNonAtomic; no quiescence or coherent event boundary; process-lifetime bookkeeping peak, not phase/native transient peak",
            "before_fixture":allocation_snapshot_record(Some(self.before_fixture)),
            "after_start_and_watch_acquisition":allocation_snapshot_record(self.after_start_and_watch_acquisition),
            "owner_ready_before_recording":allocation_snapshot_record(self.owner_ready_before_recording),
            "after_proc_recording":allocation_snapshot_record(self.after_proc_recording),
            "after_observer_completion":allocation_snapshot_record(self.after_observer_completion),
            "after_shutdown_watch_release_and_fd_sample":allocation_snapshot_record(self.after_shutdown_watch_release_and_fd_sample),
            "retention_scope":"Fixture and protocol-result values remain owned at the last phase; successful final fixture-drop snapshot is separate"
        })
    }
}

fn allocation_snapshot_record(snapshot: Option<managed_alloc::Snapshot>) -> Value {
    let Some(s) = snapshot else {
        return json!({"state":"NotReached"});
    };
    let count = |value| match value {
        managed_alloc::Count::Known(value) => json!({"state":"Known","value":value}),
        managed_alloc::Count::Unknown => {
            json!({"state":"Unknown","reason":"counter representation exhausted (including reserved MAX) or arithmetic underflow; sticky"})
        }
    };
    json!({
        "state":"Observed",
        "semantics":match s.semantics { managed_alloc::SnapshotSemantics::ConcurrentNonAtomic=>"ConcurrentNonAtomic" },
        "alloc_successes":count(s.alloc_successes),
        "alloc_failures":count(s.alloc_failures),
        "zeroed_successes":count(s.zeroed_successes),
        "zeroed_failures":count(s.zeroed_failures),
        "realloc_successes":count(s.realloc_successes),
        "realloc_failures":count(s.realloc_failures),
        "dealloc_calls":count(s.dealloc_calls),
        "allocation_requested_bytes":count(s.allocation_requested_bytes),
        "allocation_success_bytes":count(s.allocation_success_bytes),
        "realloc_requested_bytes":count(s.realloc_requested_bytes),
        "realloc_grown_bytes":count(s.realloc_grown_bytes),
        "realloc_shrunk_bytes":count(s.realloc_shrunk_bytes),
        "deallocated_bytes":count(s.deallocated_bytes),
        "live_requested_bytes":count(s.live_requested_bytes),
        "peak_recorded_live_requested_bytes":count(s.peak_recorded_live_requested_bytes)
    })
}

struct Inputs {
    members: u32,
    records: PathBuf,
    patience: Duration,
}

impl Inputs {
    fn supplied() -> Self {
        let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("supply {name}"));
        let members = required("SHARDS_OUTPUT_COST_MEMBERS").parse::<u32>().unwrap();
        assert!(
            members.checked_add(1).is_some(),
            "files-only declaration and members must fit u32"
        );
        assert!(
            usize::try_from(members).unwrap().checked_mul(2).is_some(),
            "channel count must fit usize"
        );
        let patience = Duration::from_millis(required("SHARDS_OUTPUT_COST_DEADLINE_MS").parse().unwrap());
        assert!(
            !patience.is_zero(),
            "explicit finite positive phase deadline required"
        );
        assert!(
            Instant::now().checked_add(patience).is_some(),
            "representable phase deadline"
        );
        let records = PathBuf::from(required("SHARDS_OUTPUT_COST_RECORDS"));
        assert!(records.is_absolute(), "records must be an absolute new directory");
        std::fs::create_dir(&records).unwrap();
        Self {
            members,
            records,
            patience,
        }
    }

    fn write(&self, name: &str, value: &Value) {
        std::fs::write(self.records.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }
}

struct Member {
    id: u32,
    label: String,
    token: String,
}

enum Ready {
    NoManagedMembers {
        run: String,
    },
    OwnerChannels {
        run: String,
        endpoints: usize,
        unique_addresses: usize,
    },
}

impl Ready {
    fn record(&self) -> Value {
        match self {
            Self::NoManagedMembers { run } => {
                json!({"state":"NoManagedMembers","run":run,"endpoints":0,"proof":"detached run + main exact progress; managed-output record absent"})
            }
            Self::OwnerChannels {
                run,
                endpoints,
                unique_addresses,
            } => {
                json!({"state":"OwnerChannelsReady","run":run,"endpoints":endpoints,"unique_addresses":unique_addresses,"proof":"each scoped channel exact bytes received; all except member1 stdout ACK sent; main exact progress"})
            }
        }
    }
}

fn left(until: Instant) -> Duration {
    until.saturating_duration_since(Instant::now())
}

fn save_command(inputs: &Inputs, name: &str, result: &common::Run) {
    inputs.write(
        name,
        &json!({"status":result.status,"stdout":result.stdout,"stderr":result.stderr}),
    );
}

fn start(fixture: &mut Fixture, inputs: &Inputs, until: Instant) -> (Ready, Vec<Member>) {
    let (image, _) = common::served();
    let files = fixture.context.join("files");
    std::fs::create_dir(&files).unwrap();
    std::fs::write(files.join("asset"), b"files-only").unwrap();
    let mut agentfile = format!("FROM {image}\nAGENT assets FROM ./files\n");
    let mut members = Vec::new();
    // managed_plan assigns all AGENT IDs before HARNESS IDs, not source-line order.
    let agents = inputs.members.div_ceil(2);
    for ordinal in 0..inputs.members {
        let id = ordinal.checked_add(1).unwrap();
        let token = format!("member{id}");
        let kind = if ordinal < agents { "agent" } else { "harness" };
        let directory = fixture.context.join(&token);
        std::fs::create_dir(&directory).unwrap();
        common::program_copy(common::test_guest(), &directory.join("member"));
        std::fs::write(
            directory.join(format!("{kind}.json")),
            serde_json::to_vec(&json!({"entrypoint":"member","options":["managed-capture",token]})).unwrap(),
        )
        .unwrap();
        agentfile.push_str(&format!("{} {token} FROM ./{token}\n", kind.to_ascii_uppercase()));
        members.push(Member {
            id,
            label: format!("{kind} {token}"),
            token,
        });
    }
    std::fs::write(fixture.context.join("Agentfile"), &agentfile).unwrap();
    std::fs::write(inputs.records.join("Agentfile"), agentfile).unwrap();
    let built = fixture.command(
        &[
            "build",
            "image",
            "-t",
            "owner-output:1",
            fixture.context.to_str().unwrap(),
        ],
        left(until),
    );
    save_command(inputs, "build.json", &built);
    assert_eq!(built.status, Some(0), "{built}");
    let started = fixture.command(
        &[
            "run",
            "vm",
            "-d",
            "--name",
            "owner-output",
            "owner-output:1",
            "sleep",
        ],
        left(until),
    );
    save_command(inputs, "start.json", &started);
    assert_eq!(started.status, Some(0), "{started}");
    let run = started.stdout.trim().to_owned();
    assert!(
        !run.is_empty() && run.bytes().all(|b| b.is_ascii_hexdigit()),
        "run identity"
    );
    let record = fixture.home.join("containers").join(&run).join("managed-output");
    let ready = if inputs.members == 0 {
        assert!(
            matches!(std::fs::read(&record), Err(e) if e.kind()==std::io::ErrorKind::NotFound),
            "zero active members must not publish capture metadata"
        );
        Ready::NoManagedMembers { run }
    } else {
        let text = std::fs::read_to_string(&record).unwrap();
        let metadata = Metadata::parse(&text).unwrap_or_else(|| panic!("trusted scoped owner metadata"));
        assert_eq!(metadata.run, run);
        assert_eq!(metadata.server, fixture.owner.fingerprint());
        assert_eq!(metadata.owner, fixture.owner.fingerprint());
        let expected: Vec<_> = members.iter().flat_map(|m| [(m.id, 1), (m.id, 2)]).collect();
        assert_eq!(
            metadata.endpoints.keys().copied().collect::<Vec<_>>(),
            expected,
            "files-only declaration hole must remain member0"
        );
        // Published metadata contains public fingerprints and endpoint addresses only.
        std::fs::write(inputs.records.join("managed-output.ready"), text).unwrap();
        let ready = Ready::OwnerChannels {
            run,
            endpoints: metadata.endpoints.len(),
            unique_addresses: metadata
                .endpoints
                .values()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
        };
        fixture.record = Some(record);
        fixture.metadata = Some(metadata);
        ready
    };
    fixture.initial_fds = Some(std::fs::read_dir("/proc/self/fd").unwrap().count());
    (ready, members)
}

#[derive(Debug, Clone)]
enum ObservationFailure {
    Io {
        kind: std::io::ErrorKind,
        code: Option<i32>,
    },
    MalformedIdentityRecord,
    InspectFailed {
        status: Option<i32>,
    },
    MalformedVmPid,
    MissingExpectedStart,
    MalformedPinnedStat,
    DifferentIdentity {
        expected: (u32, u64),
        observed: (u32, u64),
    },
    FdMismatch {
        expected: Option<usize>,
        observed: usize,
    },
    CounterOverflow,
    NotPrepared,
}

impl ObservationFailure {
    fn io(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            code: error.raw_os_error(),
        }
    }
    fn record(&self) -> Value {
        match self {
            Self::Io { kind, code } => {
                json!({"state":"Unavailable","kind":format!("{kind:?}"),"raw_os_code":code})
            }
            Self::MalformedIdentityRecord => json!({"state":"MalformedIdentityRecord"}),
            Self::InspectFailed { status } => json!({"state":"InspectFailed","status":status}),
            Self::MalformedVmPid => json!({"state":"MalformedVmPid"}),
            Self::MissingExpectedStart => json!({"state":"MissingExpectedStart"}),
            Self::MalformedPinnedStat => json!({"state":"MalformedPinnedStat"}),
            Self::DifferentIdentity { expected, observed } => {
                json!({"state":"DifferentIdentity","expected":expected,"observed":observed})
            }
            Self::FdMismatch { expected, observed } => {
                json!({"state":"FdMismatch","expected":expected,"observed":observed})
            }
            Self::CounterOverflow => json!({"state":"CounterOverflow"}),
            Self::NotPrepared => json!({"state":"NotPrepared"}),
        }
    }
}

#[derive(serde::Serialize)]
enum LinkCoverage {
    CompleteAtEnumeration,
    Partial,
    Unavailable,
}

fn failure(error: std::io::Error) -> Value {
    ObservationFailure::io(error).record()
}

fn read_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn retain(inputs: &Inputs, stem: &str, bytes: std::io::Result<Vec<u8>>) -> Value {
    match bytes {
        Ok(bytes) => {
            std::fs::write(inputs.records.join(stem), &bytes).unwrap();
            json!({"state":"Observed","file":stem,"bytes":bytes.len()})
        }
        Err(error) => failure(error),
    }
}

fn named(path: &Path) -> Result<shards_ipc::Named, ObservationFailure> {
    let bytes = read_bytes(path).map_err(ObservationFailure::io)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| ObservationFailure::MalformedIdentityRecord)?;
    shards_ipc::Named::parse(text).ok_or(ObservationFailure::MalformedIdentityRecord)
}

fn vm(fixture: &Fixture, inputs: &Inputs, until: Instant) -> Result<shards_ipc::Named, ObservationFailure> {
    let inspected = fixture.command(
        &["inspect", "vm", "-f", "{{.State.Pid}}", "owner-output"],
        left(until),
    );
    save_command(inputs, "inspect.json", &inspected);
    if inspected.status != Some(0) {
        return Err(ObservationFailure::InspectFailed {
            status: inspected.status,
        });
    }
    let pid = inspected
        .stdout
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or(ObservationFailure::MalformedVmPid)?;
    let start = shards_ipc::started(pid).map_err(ObservationFailure::io)?;
    Ok(shards_ipc::Named {
        pid,
        start: Some(start),
    })
}

fn snapshot(inputs: &Inputs, role: &str, identity: &Result<shards_ipc::Named, ObservationFailure>) -> Value {
    let identity = match identity {
        Ok(identity) => identity,
        Err(error) => return error.record(),
    };
    // An opened proc directory keeps operations tied to its original task even if
    // that PID is reused. No ordinary pathname re-lookup between observations.
    use std::os::fd::AsRawFd as _;
    let directory = match std::fs::File::open(format!("/proc/{}", identity.pid)) {
        Ok(directory) => directory,
        Err(error) => return failure(error),
    };
    let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let before = read_bytes(&root.join("stat"));
    let before_identity = before.as_ref().ok().and_then(|b| parse::process(b));
    let expected = identity.start.map(|start| (identity.pid, start));
    if expected.is_none() || before_identity != expected {
        return json!({"state":"IdentityUnverifiedBeforeRead","pid":identity.pid,"expected_start":identity.start,"observed_identity":before_identity,"read_error":before.err().map(failure)});
    }
    let mut fields = serde_json::Map::new();
    fields.insert(
        "stat_before".into(),
        retain(inputs, &format!("{role}.stat-before"), before),
    );
    for file in ["status", "smaps_rollup", "cgroup"] {
        fields.insert(
            file.into(),
            retain(inputs, &format!("{role}.{file}"), read_bytes(&root.join(file))),
        );
    }
    let mut inodes = BTreeSet::new();
    let mut coverage = LinkCoverage::CompleteAtEnumeration;
    let fds = match std::fs::read_dir(root.join("fd")) {
        Err(error) => {
            coverage = LinkCoverage::Unavailable;
            failure(error)
        }
        Ok(entries) => {
            let mut rows = Vec::new();
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        coverage = LinkCoverage::Partial;
                        rows.push(failure(error));
                        continue;
                    }
                };
                let fd = entry.file_name().to_string_lossy().into_owned();
                match std::fs::read_link(entry.path()) {
                    Err(error) => {
                        coverage = LinkCoverage::Partial;
                        rows.push(json!({"fd":fd,"observation":failure(error)}));
                    }
                    Ok(target) => {
                        use std::os::unix::ffi::OsStrExt as _;
                        let bytes = target.as_os_str().as_bytes();
                        let inode = parse::socket_inode(bytes);
                        if let Some(inode) = inode {
                            inodes.insert(inode);
                        }
                        // Never retain ordinary pathnames, credential paths or key contents.
                        let kind = if inode.is_some() {
                            "Socket"
                        } else if bytes.starts_with(b"pipe:[") {
                            "Pipe"
                        } else if bytes.starts_with(b"anon_inode:") {
                            "Anonymous"
                        } else {
                            "OtherTargetOmitted"
                        };
                        rows.push(json!({"fd":fd,"kind":kind,"socket_inode":inode}));
                    }
                }
            }
            json!({"state":"ObservedEntriesNonAtomic","entry_count":rows.len(),"entries":rows})
        }
    };
    fields.insert("fds".into(), json!({"observations":fds,"link_coverage":coverage,"known_socket_inodes":inodes,"known_socket_inode_count":inodes.len(),"complete_socket_count":"Unverified: non-atomic enumeration; failures are explicit"}));
    for table in ["udp", "udp6"] {
        let observation = match read_bytes(&root.join("net").join(table)) {
            Err(error) => failure(error),
            Ok(bytes) => match parse::owned_udp(&bytes, &inodes) {
                None => json!({"state":"MalformedSocketTable"}),
                Some(rows) => {
                    let mut owned = Vec::new();
                    for row in &rows {
                        owned.extend_from_slice(row);
                        owned.push(b'\n');
                    }
                    json!({"retained":retain(inputs, &format!("{role}.{table}-owned"), Ok(owned)),"matched_row_count":rows.len(),"fd_link_coverage":coverage,"scope":"only rows matching known sampled owned FD socket inodes; non-atomic, not a complete live socket count"})
                }
            },
        };
        fields.insert(table.into(), observation);
    }
    let after = read_bytes(&root.join("stat"));
    let after_identity = after.as_ref().ok().and_then(|b| parse::process(b));
    fields.insert(
        "stat_after".into(),
        retain(inputs, &format!("{role}.stat-after"), after),
    );
    let state = if expected.is_some() && before_identity == expected && after_identity == expected {
        "MatchedBeforeAndAfter"
    } else {
        "IdentityUnverified"
    };
    json!({"pid":identity.pid,"expected_start":identity.start,"before_identity":before_identity,"after_identity":after_identity,"identity_state":state,"observations":fields,"atomic":false,"cgroup_kernel_attribution":"Unverified: proc membership only; no isolated per-member socket charge proof"})
}

fn fd_count() -> Result<usize, ObservationFailure> {
    let entries = std::fs::read_dir("/proc/self/fd").map_err(ObservationFailure::io)?;
    let mut count = 0usize;
    for entry in entries {
        entry.map_err(ObservationFailure::io)?;
        count = count.checked_add(1).ok_or(ObservationFailure::CounterOverflow)?;
    }
    Ok(count)
}

struct ExitFds {
    baseline: Result<usize, ObservationFailure>,
    acquired: Result<usize, ObservationFailure>,
    held: usize,
}

impl ExitFds {
    fn returned(&self, released: &Result<usize, ObservationFailure>) -> bool {
        matches!((&self.baseline,&self.acquired,released),(Ok(base),Ok(acquired),Ok(released))
            if base.checked_add(self.held)==Some(*acquired) && base==released)
    }

    fn record(&self, released: &Result<usize, ObservationFailure>) -> Value {
        let sample = |value: &Result<usize, ObservationFailure>| match value {
            Ok(count) => json!({"state":"Observed","count":count}),
            Err(error) => error.record(),
        };
        json!({"baseline":sample(&self.baseline),"with_exit_objects":sample(&self.acquired),"owned_observation_fds":self.held,"after_release":sample(released),"exact_delta_and_return":self.returned(released)})
    }
}

struct PinnedProcess {
    expected: shards_ipc::Named,
    directory: std::fs::File,
}

impl PinnedProcess {
    fn open(expected: shards_ipc::Named) -> Result<Self, ObservationFailure> {
        if expected.start.is_none() {
            return Err(ObservationFailure::MissingExpectedStart);
        }
        let directory =
            std::fs::File::open(format!("/proc/{}", expected.pid)).map_err(ObservationFailure::io)?;
        Ok(Self { expected, directory })
    }

    fn check(&self) -> Result<(), ObservationFailure> {
        use std::os::fd::AsRawFd as _;
        let bytes = std::fs::read(format!("/proc/self/fd/{}/stat", self.directory.as_raw_fd()))
            .map_err(ObservationFailure::io)?;
        let observed = parse::process(&bytes).ok_or(ObservationFailure::MalformedPinnedStat)?;
        let expected = (
            self.expected.pid,
            self.expected
                .start
                .ok_or(ObservationFailure::MissingExpectedStart)?,
        );
        if observed != expected {
            return Err(ObservationFailure::DifferentIdentity { expected, observed });
        }
        Ok(())
    }
}

#[repr(u64)]
enum HostRole {
    Daemon = 1,
    Vm = 2,
}

enum ReadinessIdentity {
    Pending,
    Matched,
    Unverified(ObservationFailure),
}

enum HostExit {
    Unavailable {
        expected: Option<shards_ipc::Named>,
        stage: &'static str,
        failure: ObservationFailure,
    },
    Watching {
        identity: PinnedProcess,
        _watch: shards_vmm::platform::ExitWatch,
        readiness: ReadinessIdentity,
    },
    ExitReady(shards_ipc::Named),
    Deadline(shards_ipc::Named),
}

impl HostExit {
    fn acquire(
        poller: &shards_vmm::platform::Poller,
        expected: &Result<shards_ipc::Named, ObservationFailure>,
        role: HostRole,
    ) -> Self {
        let acquire = || {
            let expected = expected.as_ref().map_err(|e| ("expected_identity", e.clone()))?;
            let identity = PinnedProcess::open(*expected).map_err(|e| ("open_identity", e))?;
            identity.check().map_err(|e| ("before_watch", e))?;
            let watch = poller
                .add_exit(expected.pid, role as u64)
                .map_err(|e| ("add_exit", ObservationFailure::io(e)))?;
            identity.check().map_err(|e| ("after_watch", e))?;
            Ok((identity, watch))
        };
        match acquire() {
            Ok((identity, watch)) => Self::Watching {
                identity,
                _watch: watch,
                readiness: ReadinessIdentity::Pending,
            },
            Err((stage, failure)) => Self::Unavailable {
                expected: expected.as_ref().ok().copied(),
                stage,
                failure,
            },
        }
    }

    fn retained_fds(&self) -> usize {
        // Each actual Watching value owns one proc File and one unique Linux pidfd.
        match self {
            Self::Watching { .. } => 2,
            _ => 0,
        }
    }

    fn validate_readiness(&mut self) {
        if let Self::Watching {
            identity, readiness, ..
        } = self
        {
            *readiness = match identity.check() {
                Ok(()) => ReadinessIdentity::Matched,
                Err(e) => ReadinessIdentity::Unverified(e),
            };
        }
    }

    fn before_wait(self) -> Self {
        match self {
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Pending,
                ..
            } => Self::Unavailable {
                expected: Some(identity.expected),
                stage: "final_readiness",
                failure: ObservationFailure::NotPrepared,
            },
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Unverified(failure),
                ..
            } => Self::Unavailable {
                expected: Some(identity.expected),
                stage: "final_readiness",
                failure,
            },
            state => state,
        }
    }

    fn awaiting(&self) -> bool {
        matches!(
            self,
            Self::Watching {
                readiness: ReadinessIdentity::Matched,
                ..
            }
        )
    }

    fn observed_exit(self) -> Self {
        match self {
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Matched,
                ..
            } => Self::ExitReady(identity.expected),
            state => state,
        }
    }

    fn deadline(self) -> Self {
        match self {
            Self::Watching { identity, .. } => Self::Deadline(identity.expected),
            state => state,
        }
    }

    fn wait_failed(self, failure: ObservationFailure) -> Self {
        match self {
            Self::Watching { identity, .. } => Self::Unavailable {
                expected: Some(identity.expected),
                stage: "wait_exit",
                failure,
            },
            state => state,
        }
    }

    fn record(&self) -> Value {
        let named = |n: &shards_ipc::Named| json!({"pid":n.pid,"expected_started":n.start});
        match self {
            Self::Unavailable {
                expected,
                stage,
                failure,
            } => {
                json!({"state":"Unavailable","expected_identity":expected.as_ref().map(named),"stage":stage,"failure":failure.record()})
            }
            Self::Watching {
                identity, readiness, ..
            } => {
                json!({"state":"Watching","identity":named(&identity.expected),"readiness_identity":match readiness { ReadinessIdentity::Pending=>json!({"state":"Pending"}),ReadinessIdentity::Matched=>json!({"state":"Matched"}),ReadinessIdentity::Unverified(e)=>e.record() }})
            }
            Self::ExitReady(identity) => {
                json!({"state":"ExitReadyObservedAfterOwnerStopWithinCleanupDeadline","identity":named(identity),"reaped":"Unverified"})
            }
            Self::Deadline(identity) => json!({"state":"Deadline","identity":named(identity)}),
        }
    }
}

struct ExitProof {
    poller: Result<shards_vmm::platform::Poller, ObservationFailure>,
    daemon: HostExit,
    vm: HostExit,
    fds: ExitFds,
}

impl ExitProof {
    fn acquire(
        fixture: &mut Fixture,
        daemon: &Result<shards_ipc::Named, ObservationFailure>,
        vm: &Result<shards_ipc::Named, ObservationFailure>,
    ) -> Self {
        let baseline = fd_count().and_then(|observed| {
            if fixture.initial_fds == Some(observed) {
                Ok(observed)
            } else {
                Err(ObservationFailure::FdMismatch {
                    expected: fixture.initial_fds,
                    observed,
                })
            }
        });
        let poller = shards_vmm::platform::Poller::new().map_err(ObservationFailure::io);
        let (daemon, vm) = match &poller {
            Ok(p) => (
                HostExit::acquire(p, daemon, HostRole::Daemon),
                HostExit::acquire(p, vm, HostRole::Vm),
            ),
            Err(e) => (
                HostExit::Unavailable {
                    expected: daemon.as_ref().ok().copied(),
                    stage: "poller",
                    failure: e.clone(),
                },
                HostExit::Unavailable {
                    expected: vm.as_ref().ok().copied(),
                    stage: "poller",
                    failure: e.clone(),
                },
            ),
        };
        // Existing Linux Poller owns one epoll FD; count only actual held objects.
        let held = usize::from(poller.is_ok()) + daemon.retained_fds() + vm.retained_fds();
        let acquired = fd_count();
        if let Ok(acquired) = acquired {
            fixture.initial_fds = Some(acquired);
        }
        Self {
            poller,
            daemon,
            vm,
            fds: ExitFds {
                baseline,
                acquired,
                held,
            },
        }
    }

    fn ready_record(&mut self) -> Value {
        self.daemon.validate_readiness();
        self.vm.validate_readiness();
        json!({"daemon":self.daemon.record(),"vm":self.vm.record()})
    }

    fn finish(self, until: Instant) -> (Value, bool, ExitFds) {
        let Self {
            poller,
            daemon,
            vm,
            fds,
        } = self;
        let mut hosts = (daemon.before_wait(), vm.before_wait());
        if let Ok(poller) = &poller {
            let mut ready = Vec::new();
            while hosts.0.awaiting() || hosts.1.awaiting() {
                let remaining = left(until);
                if remaining.is_zero() {
                    hosts = (hosts.0.deadline(), hosts.1.deadline());
                    break;
                }
                if let Err(error) = poller.wait(&mut ready, Some(remaining)) {
                    let failure = ObservationFailure::io(error);
                    hosts = (hosts.0.wait_failed(failure.clone()), hosts.1.wait_failed(failure));
                    break;
                }
                // Late observation is conservatively Deadline, not a guessed exit time.
                if Instant::now() > until {
                    hosts = (hosts.0.deadline(), hosts.1.deadline());
                    break;
                }
                if ready.contains(&(HostRole::Daemon as u64)) {
                    hosts = (hosts.0.observed_exit(), hosts.1);
                }
                if ready.contains(&(HostRole::Vm as u64)) {
                    hosts = (hosts.0, hosts.1.observed_exit());
                }
                // Consumed watches close now, removing level-triggered readiness.
                // Empty/EINTR results re-enter only this same deadline-bound event wait.
            }
        }
        let passed = matches!(hosts.0, HostExit::ExitReady(_)) && matches!(hosts.1, HostExit::ExitReady(_));
        let record = json!({"daemon":hosts.0.record(),"vm":hosts.1.record(),"both_valid_exit_events":passed,"semantics":"exit readiness only; reaping, exit status/time/causality, full-population and kernel return Unverified"});
        drop(hosts);
        drop(poller);
        (record, passed, fds)
    }
}

fn run(
    fixture: &mut Fixture,
    inputs: &Inputs,
    until: Instant,
    exits: &mut Option<ExitProof>,
    allocations: &mut AllocationPhases,
    faults: &mut NativeFaultPhases,
) -> Ready {
    let (ready, members) = start(fixture, inputs, until);
    let daemon = named(&fixture.home.join(shards_ipc::PID));
    let vm = vm(fixture, inputs, until);
    *exits = Some(ExitProof::acquire(fixture, &daemon, &vm));
    allocations.after_start_and_watch_acquisition = Some(MANAGED_ALLOC.snapshot());
    let observer = shards_ipc::started(std::process::id())
        .map(|start| shards_ipc::Named {
            pid: std::process::id(),
            start: Some(start),
        })
        .map_err(ObservationFailure::io);
    faults.after_start_and_watch_acquisition = Some(NativeFaultPhase::sample(exits.as_ref(), &observer));
    let mut channels = Vec::new();
    if let Ready::OwnerChannels { .. } = &ready {
        // Establish every authenticated observer before consuming any channel.
        for member in &members {
            for channel in [Channel::Stdout, Channel::Stderr] {
                let index = fixture.observe(member.id, channel, None, false, until);
                channels.push((member, index, channel));
            }
        }
        for (member, index, channel) in &channels {
            let (start, end) =
                fixture.observers[*index].exact(&member.label, &payload(&member.token, *channel), until);
            let held = member.id == 1 && *channel == Channel::Stdout;
            if !held {
                fixture.observers[*index]
                    .sender
                    .send(Command::Handler(Ask::Ack(end)))
                    .unwrap();
            }
            fixture
                .observed
                .insert((member.id, *channel as u8), (end, if held { start } else { end }));
        }
    }
    fixture.main_progress(left(until));
    allocations.owner_ready_before_recording = Some(MANAGED_ALLOC.snapshot());
    faults.owner_ready_before_recording = Some(NativeFaultPhase::sample(exits.as_ref(), &observer));
    if let Some(proof) = exits {
        inputs.write("exit-watches.ready.json", &proof.ready_record());
    }
    inputs.write("ready.json", &ready.record());
    let sampled = json!({"checkpoint":ready.record(),"daemon":snapshot(inputs,"daemon",&daemon),"vm":snapshot(inputs,"vm",&vm),"observer_parent":snapshot(inputs,"observer",&observer),"measurement_cost":"one-shot proc reads/serialization and observer drivers are fixture costs; samples are not atomic"});
    inputs.write("snapshots.json", &sampled);
    allocations.after_proc_recording = Some(MANAGED_ALLOC.snapshot());
    faults.after_proc_recording = Some(NativeFaultPhase::sample(exits.as_ref(), &observer));
    for (member, index, channel) in &channels {
        let observer = &mut fixture.observers[*index];
        if member.id == 1 && *channel == Channel::Stdout {
            observer.sender.send(Command::Handler(Ask::Cancel)).unwrap();
            observer.join(until).unwrap();
        } else {
            // A repeated consumed cursor is legal; FIN proves processing before close.
            let wire = fixture.observed[&(member.id, *channel as u8)].0;
            observer.ack_and_finish(wire, until);
        }
    }
    fixture.main_progress(left(until));
    allocations.after_observer_completion = Some(MANAGED_ALLOC.snapshot());
    faults.after_observer_completion = Some(NativeFaultPhase::sample(exits.as_ref(), &observer));
    inputs.write("control.json", &json!({"state":"MainProgressAfterObserverCancel","stalled_member":if inputs.members>0 { Some(1) } else { None },"members":inputs.members,"exact_payload_channels":channels.len(),"terminal_consumption":"verified after owner stop; before this point non-held ACKs were sent"}));
    ready
}

#[test]
#[ignore = "coordinator-only explicit member count/deadline/records; real VM resource observations"]
fn owner_output_resources_at_explicit_member_count() {
    let mut startup = StartupAllocations::new();
    let inputs = Inputs::supplied();
    startup.supplied_inputs_ready = Some(MANAGED_ALLOC.snapshot());
    assert!(
        !common::cannot_run_vms(),
        "resource fixture requires a real supported VM host"
    );
    let until = Instant::now().checked_add(inputs.patience).unwrap();
    let artifact = |path: &Path, phase: &mut StartupArtifactAllocations| {
        phase.after_resolution = Some(MANAGED_ALLOC.snapshot());
        let value = json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
        phase.artifact_value_ready = Some(MANAGED_ALLOC.snapshot());
        value
    };
    inputs.write(
        "inputs.json",
        &json!({
            "members":inputs.members,
            "phase_deadline_ms":inputs.patience.as_millis(),
            "source":option_env!("SHARDS_OUTPUT_COST_SOURCE_REVISION"),
            "fixture":artifact(
                &{
                    startup.fixture.before_resolution = Some(MANAGED_ALLOC.snapshot());
                    std::env::current_exe().unwrap()
                },
                &mut startup.fixture
            ),
            "cli":artifact(
                {
                    startup.cli.before_resolution = Some(MANAGED_ALLOC.snapshot());
                    common::shards()
                },
                &mut startup.cli
            ),
            "testguest":artifact(
                {
                    startup.testguest.before_resolution = Some(MANAGED_ALLOC.snapshot());
                    common::test_guest()
                },
                &mut startup.testguest
            ),
            "kernel":artifact(
                {
                    startup.kernel.before_resolution = Some(MANAGED_ALLOC.snapshot());
                    common::kernel()
                },
                &mut startup.kernel
            ),
            "init":artifact(
                {
                    startup.init.before_resolution = Some(MANAGED_ALLOC.snapshot());
                    common::guest_init()
                },
                &mut startup.init
            ),
            "topology":"EXPERIMENTAL2N; unadopted",
            "root_cost_comparison":"not executed by implementer"
        }),
    );
    startup.inputs_recorded = Some(MANAGED_ALLOC.snapshot());
    let mut allocations = AllocationPhases::new();
    let mut faults = NativeFaultPhases::default();
    let mut fixture = Fixture::new();
    // None means acquisition was not reached. Some owns all observation resources
    // outside the protocol unwind boundary, including partial acquisition failures.
    let mut exits = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(
            &mut fixture,
            &inputs,
            until,
            &mut exits,
            &mut allocations,
            &mut faults,
        )
    }));
    // Cleanup is a separate explicitly recorded finite phase, using caller patience.
    let cleanup_until = Instant::now().checked_add(inputs.patience).unwrap();
    let stopped = fixture.shutdown(cleanup_until);
    let (exit_record, exit_passed, fds) = match exits {
        Some(proof) => {
            let (record, passed, fds) = proof.finish(cleanup_until);
            (record, passed, Some(fds))
        }
        None => (ObservationFailure::NotPrepared.record(), false, None),
    };
    // Every watch, identity directory and poller is dropped before total FD sampling.
    faults.after_cleanup = Some(NativeFaultPhase::after_cleanup());
    let released = fd_count();
    allocations.after_shutdown_watch_release_and_fd_sample = Some(MANAGED_ALLOC.snapshot());
    let fd_returned = fds.as_ref().is_some_and(|fds| fds.returned(&released));
    let fd_record = fds
        .as_ref()
        .map(|fds| fds.record(&released))
        .unwrap_or_else(|| ObservationFailure::NotPrepared.record());
    inputs.write("teardown.json", &json!({"owner_stop_and_observer_join":stopped,"protocol_completed":result.is_ok(),"cleanup_phase_deadline_ms":inputs.patience.as_millis(),"host_process_exit":exit_record,"exit_observation_fds":fd_record,"host_process_exit_and_kernel_resource_return":"See held exit readiness proof; kernel/full-population return remain Unverified"}));
    inputs.write("allocation-phases.json", &allocations.record());
    inputs.write("native-fault-phases.json", &faults.record());
    inputs.write("allocation-startup.json", &startup.record());
    assert!(fd_returned, "exit observation exact FD delta/return: {fd_record}");
    assert!(
        exit_passed,
        "both validated owned exit watches must report readiness before cleanup deadline: {exit_record}"
    );
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let ready = result.unwrap();
    if let Ready::OwnerChannels { .. } = ready {
        let terminal: Vec<_> = fixture.observed.iter().map(|(&(member,channel),expected)| {
            let actual=fixture.terminal(member,Channel::try_from(channel).unwrap());
            assert_eq!(&actual,expected);
            json!({"member":member,"channel":channel,"accepted":actual.0,"consumed":actual.1,"tail":"unknown"})
        }).collect();
        inputs.write(
            "terminal.json",
            &json!({"state":"OwnerChannelsAborted","channels":terminal}),
        );
    } else {
        inputs.write(
            "terminal.json",
            &json!({"state":"NoManagedMembers","managed_channels":0}),
        );
    }
    drop(fixture);
    let final_snapshot = MANAGED_ALLOC.snapshot();
    inputs.write("allocation-final.json", &json!({"phase":"after_terminal_records_and_fixture_drop","scope":"observer/test parent only; other libtest/fixture/input/result values can remain live; includes all prior record serialization","snapshot":allocation_snapshot_record(Some(final_snapshot))}));
}
